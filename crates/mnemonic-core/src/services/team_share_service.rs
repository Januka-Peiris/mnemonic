//! Sharing of workspaces and live agent chats between teammates on the same
//! Tailscale network, and the few actions a teammate may take in a chat. Each Mnemonic serves its own state (see
//! `team_share_server`); there is no central server.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agent_client_protocol::schema as acp;
use serde::{Deserialize, Serialize};

use crate::services::team_files_service::{TeamBatchOutcome, TeamChangeBatch, TeamFileManifest};

/// Bumped whenever a change to the wire types would break an older peer.
pub const TEAM_PROTOCOL_VERSION: u32 = 5;
/// Chats travel as the ACP crate's own types, so peers on different crate
/// versions are treated as speaking different protocols.
pub const TEAM_ACP_CRATE_VERSION: &str = "0.14.0";
pub const DEFAULT_TEAM_PORT: u16 = 47231;
/// Sent on every action, so a web page can't post one without a CORS
/// preflight the share server never answers.
pub const TEAM_REQUEST_HEADER: &str = "X-Mnemonic-Team";
/// The teammate's git identity as `Name <email>`, sent with agent requests
/// so commits their agent makes in the sharer's workspace carry their name.
pub const TEAM_GIT_AUTHOR_HEADER: &str = "X-Mnemonic-Git-Author";

/// The MCP endpoint a teammate's agent uses to work in a shared workspace.
pub fn agent_mcp_url(base_url: &str, workspace_id: &str) -> String {
    format!("{base_url}/v1/workspaces/{workspace_id}/agent")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamHello {
    pub name: String,
    pub protocol: u32,
    /// Defaulted so a peer from before protocol 2 is reported as a
    /// different version rather than as an unreadable answer.
    #[serde(default)]
    pub acp_crate: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamAgentState {
    #[default]
    Idle,
    Working,
    WaitingForApproval,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamChat {
    pub id: String,
    pub title: String,
    pub agent: String,
    /// Whether the chat can be read: it is loaded in the sharer's app, or
    /// was earlier, or has a transcript saved from an earlier session.
    pub open: bool,
    /// Whether the chat's agent is running in the sharer's app, so a
    /// teammate can stop it or message it.
    #[serde(default)]
    pub live: bool,
    /// When the chat's last turn ended, in unix milliseconds. Only the phone
    /// feed sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_ended_at: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub last_turn_errored: bool,
    /// Whether a permission prompt is pending. Only the phone feed sets it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub waiting: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamWorkspace {
    pub id: String,
    pub project: String,
    pub name: String,
    pub branch: String,
    #[serde(default)]
    pub pr_number: Option<i64>,
    /// One of "open", "ready", "conflicts", "failed", "merged", "closed".
    #[serde(default)]
    pub pr_state: Option<String>,
    #[serde(default)]
    pub changed_files: u32,
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
    #[serde(default)]
    pub agent_state: TeamAgentState,
    /// Seconds since the agent last did something, when known.
    #[serde(default)]
    pub idle_seconds: Option<u64>,
    #[serde(default)]
    pub chats: Vec<TeamChat>,
    /// Whether the sharer lets teammates act in this workspace's chats.
    #[serde(default)]
    pub step_in: bool,
    /// Whether the sharer lets teammates copy this workspace's files.
    #[serde(default)]
    pub share_files: bool,
    /// Whether teammates may also send changes back. Implies `share_files`.
    #[serde(default)]
    pub edit_files: bool,
    #[serde(default)]
    pub remote_url: Option<String>,
    #[serde(default)]
    pub base_branch: Option<String>,
    /// The sharer's skip list for the workspace's files.
    #[serde(default)]
    pub skip_patterns: Vec<String>,
    /// Whether teammates' agents may work in this workspace's folder.
    #[serde(default)]
    pub agents: bool,
    /// The workspace's folder on the sharer's machine. Set only when `agents`.
    #[serde(default)]
    pub root_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamWorkspaceList {
    pub workspaces: Vec<TeamWorkspace>,
}

/// One entry of a chat transcript.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamEntry {
    /// Applied in order to the end of a thread, these rebuild one entry.
    Updates { updates: Vec<acp::SessionUpdate> },
    CompletedPlan { entries: Vec<acp::PlanEntry> },
    Compaction {
        #[serde(default)]
        summary: Option<String>,
    },
    /// An entry kind this version doesn't know, so a newer sharer can add
    /// kinds without breaking older viewers.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamEntryChange {
    pub index: usize,
    pub entry: TeamEntry,
}

/// What changed in a chat since the viewer's cursor. When `reset` is set the
/// viewer drops what it has and `changes` holds every entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamChatUpdate {
    pub cursor: String,
    pub reset: bool,
    pub len: usize,
    pub changes: Vec<TeamEntryChange>,
    /// The chat's current plan, sent with every update.
    #[serde(default)]
    pub plan: Vec<acp::PlanEntry>,
    /// Whether the agent is mid-turn.
    #[serde(default)]
    pub generating: bool,
    /// Permission requests the agent is waiting on.
    #[serde(default)]
    pub permissions: Vec<TeamPermissionRequest>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamPermissionRequest {
    pub tool_call_id: String,
    pub options: Vec<acp::PermissionOption>,
}

/// A teammate who passed the allow list.
#[derive(Clone, Debug, PartialEq)]
pub struct TeamCaller {
    pub login: String,
    /// Their Tailscale display name, or their login when there isn't one.
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TeamAction {
    pub chat_id: String,
    /// The caller's display name, which they choose themselves.
    pub from: String,
    pub login: String,
    pub kind: TeamActionKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TeamActionKind {
    Stop,
    Permission { tool_call_id: String, option_id: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamActionOutcome {}

/// Why an action wasn't carried out, mapped to 400, 403, 404, 409 and 500.
#[derive(Clone, Debug, PartialEq)]
pub enum TeamActionError {
    /// The request itself can't be carried out, as a batch naming a path
    /// a teammate may not write.
    Invalid(String),
    NotAllowed(String),
    NotLoaded,
    NothingToDo(String),
    Failed(String),
}

pub type ActionHandler =
    Arc<dyn Fn(TeamAction) -> Result<TeamActionOutcome, TeamActionError> + Send + Sync>;

/// A teammate's batch of file changes, held or applied by the sharer's app.
#[derive(Clone, Debug, PartialEq)]
pub struct TeamBatchRequest {
    pub workspace_id: String,
    pub from: String,
    pub login: String,
    pub batch: TeamChangeBatch,
}

pub type BatchHandler =
    Arc<dyn Fn(TeamBatchRequest) -> Result<TeamBatchOutcome, TeamActionError> + Send + Sync>;

/// The workspace's whole file manifest, replaced rather than patched on
/// each change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamFileUpdate {
    pub cursor: String,
    /// None when the wait ended with the viewer's cursor still current.
    pub manifest: Option<TeamFileManifest>,
}

/// Where to reach a teammate: a Tailscale machine name or address, with an
/// optional `:port`.
pub fn team_base_url(host: &str, default_port: u16) -> Result<String, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("The teammate has no host".to_string());
    }
    if host.contains('/') || host.contains('@') || host.contains(char::is_whitespace) {
        return Err(format!("\"{host}\" isn't a machine name"));
    }
    let has_port = match host.rsplit_once(':') {
        Some((_, port)) => !host.contains("::") && port.parse::<u16>().is_ok(),
        None => false,
    };
    Ok(if has_port {
        format!("http://{host}")
    } else {
        format!("http://{host}:{default_port}")
    })
}

#[derive(Default)]
struct ChatFeed {
    /// Bumped when entries are removed or the thread is replaced, which
    /// invalidates every viewer's cursor.
    generation: u64,
    entries: Vec<(TeamEntry, u64)>,
    plan: Vec<acp::PlanEntry>,
    plan_changed_at: u64,
    generating: bool,
    permissions: Vec<TeamPermissionRequest>,
    activity_changed_at: u64,
}

struct HubState {
    /// Random per process, so a viewer's cursor from before a restart of
    /// the sharer's app is never mistaken for a current one.
    epoch: u64,
    version: u64,
    workspaces: Vec<TeamWorkspace>,
    chats: HashMap<String, ChatFeed>,
    step_in_chats: HashSet<String>,
    /// Each workspace's manifest and the hub version it last changed at.
    manifests: HashMap<String, (TeamFileManifest, u64)>,
    last_request: Option<Instant>,
}

struct HubInner {
    state: Mutex<HubState>,
    changed: Condvar,
}

/// What this Mnemonic shares, written by the app and read by the server's
/// request threads.
#[derive(Clone)]
pub struct TeamShareHub {
    inner: Arc<HubInner>,
}

impl Default for TeamShareHub {
    fn default() -> Self {
        Self::new()
    }
}

/// How long a long-poll waits after the first change for more, so a
/// streaming reply goes out in a few batches rather than one per token.
const COALESCE_CHANGES: Duration = Duration::from_millis(100);

impl TeamShareHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HubInner {
                state: Mutex::new(HubState {
                    epoch: u64::from(rand::random::<u32>()),
                    version: 0,
                    workspaces: Vec::new(),
                    chats: HashMap::new(),
                    step_in_chats: HashSet::new(),
                    manifests: HashMap::new(),
                    last_request: None,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HubState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn set_workspaces(&self, workspaces: Vec<TeamWorkspace>) {
        let mut state = self.lock();
        if state.workspaces != workspaces {
            state.workspaces = workspaces;
        }
    }

    pub fn workspaces(&self) -> Vec<TeamWorkspace> {
        self.lock().workspaces.clone()
    }

    pub fn set_file_manifest(&self, workspace_id: &str, manifest: TeamFileManifest) {
        let mut state = self.lock();
        if state
            .manifests
            .get(workspace_id)
            .is_some_and(|(current, _)| *current == manifest)
        {
            return;
        }
        state.version += 1;
        let version = state.version;
        state
            .manifests
            .insert(workspace_id.to_string(), (manifest, version));
        self.inner.changed.notify_all();
    }

    pub fn file_manifest(&self, workspace_id: &str) -> Option<TeamFileManifest> {
        self.lock()
            .manifests
            .get(workspace_id)
            .map(|(manifest, _)| manifest.clone())
    }

    pub fn remove_file_manifest(&self, workspace_id: &str) {
        let mut state = self.lock();
        if state.manifests.remove(workspace_id).is_some() {
            self.inner.changed.notify_all();
        }
    }

    /// The manifest when it differs from the one `cursor` names, waiting up
    /// to `wait` for that to be so. A cursor from another run of the app
    /// never matches, so a reconnect always gets the current manifest.
    pub fn wait_file_manifest(
        &self,
        workspace_id: &str,
        cursor: Option<&str>,
        wait: Duration,
    ) -> Option<TeamFileUpdate> {
        let deadline = Instant::now() + wait;
        let mut state = self.lock();
        loop {
            let (manifest, changed_at) = state.manifests.get(workspace_id)?;
            let current = format!("{}.{}", state.epoch, changed_at);
            let unchanged = cursor == Some(current.as_str());
            let now = Instant::now();
            if !unchanged || now >= deadline {
                return Some(TeamFileUpdate {
                    manifest: (!unchanged).then(|| manifest.clone()),
                    cursor: current,
                });
            }
            state = self
                .inner
                .changed
                .wait_timeout(state, deadline - now)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }

    pub fn has_workspace(&self, workspace_id: &str) -> bool {
        self.lock()
            .workspaces
            .iter()
            .any(|workspace| workspace.id == workspace_id)
    }

    pub fn note_request(&self) {
        self.lock().last_request = Some(Instant::now());
    }

    /// Whether a teammate has asked for anything recently, so work done only
    /// for them (like refreshing diff counts) can stop when nobody watches.
    pub fn has_viewers_within(&self, window: Duration) -> bool {
        self.lock()
            .last_request
            .is_some_and(|at| at.elapsed() <= window)
    }

    pub fn has_chat(&self, chat_id: &str) -> bool {
        self.lock().chats.contains_key(chat_id)
    }

    pub fn chat_len(&self, chat_id: &str) -> Option<usize> {
        self.lock()
            .chats
            .get(chat_id)
            .map(|feed| feed.entries.len())
    }

    /// Replaces a chat's whole transcript, as when its thread is first seen
    /// or replaced.
    pub fn replace_chat(&self, chat_id: &str, entries: Vec<TeamEntry>) {
        let mut state = self.lock();
        state.version += 1;
        let version = state.version;
        let feed = state.chats.entry(chat_id.to_string()).or_default();
        feed.generation += 1;
        feed.entries = entries.into_iter().map(|entry| (entry, version)).collect();
        self.inner.changed.notify_all();
    }

    /// Sets one entry, appending when `index` is the current length. Returns
    /// false when the entry can't be placed (unknown chat, or a gap), in
    /// which case the caller should replace the whole chat.
    pub fn set_chat_entry(&self, chat_id: &str, index: usize, entry: TeamEntry) -> bool {
        let mut state = self.lock();
        let next_version = state.version + 1;
        let Some(feed) = state.chats.get_mut(chat_id) else {
            return false;
        };
        match index.cmp(&feed.entries.len()) {
            std::cmp::Ordering::Less => {
                if feed.entries[index].0 == entry {
                    return true;
                }
                feed.entries[index] = (entry, next_version);
            }
            std::cmp::Ordering::Equal => feed.entries.push((entry, next_version)),
            std::cmp::Ordering::Greater => return false,
        }
        state.version = next_version;
        self.inner.changed.notify_all();
        true
    }

    /// Sets a chat's current plan. Returns false when the chat isn't known,
    /// in which case the caller should replace the whole chat.
    pub fn set_chat_plan(&self, chat_id: &str, entries: Vec<acp::PlanEntry>) -> bool {
        let mut state = self.lock();
        let next_version = state.version + 1;
        let Some(feed) = state.chats.get_mut(chat_id) else {
            return false;
        };
        if feed.plan == entries {
            return true;
        }
        feed.plan = entries;
        feed.plan_changed_at = next_version;
        state.version = next_version;
        self.inner.changed.notify_all();
        true
    }

    /// Sets whether a chat's agent is mid-turn and what it waits on. Returns
    /// false when the chat isn't known.
    pub fn set_chat_activity(
        &self,
        chat_id: &str,
        generating: bool,
        permissions: Vec<TeamPermissionRequest>,
    ) -> bool {
        let mut state = self.lock();
        let next_version = state.version + 1;
        let Some(feed) = state.chats.get_mut(chat_id) else {
            return false;
        };
        if feed.generating == generating && feed.permissions == permissions {
            return true;
        }
        feed.generating = generating;
        feed.permissions = permissions;
        feed.activity_changed_at = next_version;
        state.version = next_version;
        self.inner.changed.notify_all();
        true
    }

    /// The chats teammates may act in, as decided by the sharer.
    pub fn set_step_in_chats(&self, chat_ids: HashSet<String>) {
        self.lock().step_in_chats = chat_ids;
    }

    pub fn allows_step_in(&self, chat_id: &str) -> bool {
        self.lock().step_in_chats.contains(chat_id)
    }

    pub fn remove_chat(&self, chat_id: &str) {
        if self.lock().chats.remove(chat_id).is_some() {
            self.inner.changed.notify_all();
        }
    }

    pub fn retain_chats(&self, mut keep: impl FnMut(&str) -> bool) {
        let mut state = self.lock();
        let before = state.chats.len();
        state.chats.retain(|chat_id, _| keep(chat_id));
        if state.chats.len() != before {
            self.inner.changed.notify_all();
        }
    }

    /// What changed since `cursor`, or None when the chat isn't loaded.
    pub fn chat_update(&self, chat_id: &str, cursor: Option<&str>) -> Option<TeamChatUpdate> {
        let state = self.lock();
        Self::update_locked(&state, chat_id, cursor).map(|(update, _)| update)
    }

    /// Like `chat_update`, but waits up to `timeout` for something to change.
    /// Returns an update with no changes when nothing did.
    pub fn wait_chat_update(
        &self,
        chat_id: &str,
        cursor: Option<&str>,
        timeout: Duration,
    ) -> Option<TeamChatUpdate> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        let mut waited = false;
        loop {
            let (update, changed) = Self::update_locked(&state, chat_id, cursor)?;
            if changed {
                if waited {
                    drop(state);
                    std::thread::sleep(COALESCE_CHANGES);
                    return self.chat_update(chat_id, cursor);
                }
                return Some(update);
            }
            let now = Instant::now();
            if now >= deadline {
                return Some(update);
            }
            state = self
                .inner
                .changed
                .wait_timeout(state, deadline - now)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
            waited = true;
        }
    }

    /// The update since `cursor`, and whether anything in it changed.
    fn update_locked(
        state: &HubState,
        chat_id: &str,
        cursor: Option<&str>,
    ) -> Option<(TeamChatUpdate, bool)> {
        let feed = state.chats.get(chat_id)?;
        let parsed = cursor.and_then(parse_cursor);
        let since = match parsed {
            Some((epoch, generation, version))
                if epoch == state.epoch
                    && generation == feed.generation
                    && version <= state.version =>
            {
                Some(version)
            }
            _ => None,
        };
        let changes: Vec<TeamEntryChange> = feed
            .entries
            .iter()
            .enumerate()
            .filter(|(_, (_, changed_at))| since.is_none_or(|since| *changed_at > since))
            .map(|(index, (entry, _))| TeamEntryChange {
                index,
                entry: entry.clone(),
            })
            .collect();
        let changed = !changes.is_empty()
            || since.is_none_or(|since| {
                feed.plan_changed_at > since || feed.activity_changed_at > since
            });
        let update = TeamChatUpdate {
            cursor: format!("{}.{}.{}", state.epoch, feed.generation, state.version),
            reset: since.is_none(),
            len: feed.entries.len(),
            changes,
            plan: feed.plan.clone(),
            generating: feed.generating,
            permissions: feed.permissions.clone(),
        };
        Some((update, changed))
    }
}

fn parse_cursor(cursor: &str) -> Option<(u64, u64, u64)> {
    let mut parts = cursor.split('.');
    let epoch = parts.next()?.parse().ok()?;
    let generation = parts.next()?.parse().ok()?;
    let version = parts.next()?.parse().ok()?;
    parts
        .next()
        .is_none()
        .then_some((epoch, generation, version))
}

/// Who a request came from, by Tailscale login. Behind a trait so the server
/// can be tested without Tailscale.
pub trait CallerIdentity: Send + Sync {
    /// The caller, None when the address isn't a known user (such as a
    /// tagged machine), or an error when it couldn't be checked.
    fn caller(&self, address: IpAddr) -> Result<Option<TeamCaller>, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct TailscaleSelf {
    pub ip: Ipv4Addr,
    pub login: String,
    pub display_name: String,
    /// The MagicDNS name without its trailing dot.
    pub dns_name: String,
}

const TAILSCALE_LOCATIONS: &[&str] = &[
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
    "/usr/bin/tailscale",
];

/// The Tailscale CLI. Apps opened from the Finder don't get the shell's
/// PATH, so the usual install locations are checked as well.
fn tailscale_binary() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("tailscale"))
            .find(|candidate| candidate.is_file())
    });
    on_path.or_else(|| {
        TAILSCALE_LOCATIONS
            .iter()
            .map(PathBuf::from)
            .find(|candidate| candidate.is_file())
    })
}

// Only ever called from the share server's own threads, never the UI thread.
#[allow(clippy::disallowed_methods)]
fn run_tailscale(args: &[&str]) -> Result<Vec<u8>, String> {
    let binary = tailscale_binary().ok_or_else(|| "Tailscale isn't installed".to_string())?;
    let output = std::process::Command::new(&binary)
        .args(args)
        .output()
        .map_err(|err| format!("Couldn't run Tailscale: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("tailscale {} failed", args.join(" "))
        } else {
            stderr
        });
    }
    Ok(output.stdout)
}

#[derive(Deserialize)]
struct StatusJson {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "Self")]
    this_node: Option<StatusNode>,
    #[serde(rename = "User", default)]
    users: Option<HashMap<String, StatusUser>>,
    #[serde(rename = "Peer", default)]
    peers: Option<HashMap<String, StatusNode>>,
}

#[derive(Deserialize)]
struct StatusNode {
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Option<Vec<String>>,
    #[serde(rename = "UserID", default)]
    user_id: Option<i64>,
    #[serde(rename = "HostName", default)]
    host_name: String,
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "OS", default)]
    os: String,
    #[serde(rename = "Online", default)]
    online: bool,
}

#[derive(Deserialize)]
struct StatusUser {
    #[serde(rename = "LoginName", default)]
    login_name: String,
    #[serde(rename = "DisplayName", default)]
    display_name: String,
}

/// This machine's Tailscale address and user, or why there isn't one.
pub fn tailscale_self() -> Result<TailscaleSelf, String> {
    let output = run_tailscale(&["status", "--json", "--peers=false"])?;
    parse_tailscale_status(&output)
}

fn parse_tailscale_status(output: &[u8]) -> Result<TailscaleSelf, String> {
    let status: StatusJson = serde_json::from_slice(output)
        .map_err(|err| format!("Couldn't read Tailscale's status: {err}"))?;
    if status.backend_state != "Running" {
        return Err("Tailscale isn't connected".to_string());
    }
    let node = status
        .this_node
        .ok_or_else(|| "Tailscale didn't report this machine".to_string())?;
    let ip = node
        .tailscale_ips
        .unwrap_or_default()
        .iter()
        .find_map(|ip| ip.parse::<Ipv4Addr>().ok())
        .ok_or_else(|| "This machine has no Tailscale IPv4 address".to_string())?;
    let user = node
        .user_id
        .and_then(|id| status.users.as_ref()?.get(&id.to_string()));
    Ok(TailscaleSelf {
        ip,
        login: user.map(|user| user.login_name.clone()).unwrap_or_default(),
        display_name: user
            .map(|user| user.display_name.clone())
            .unwrap_or_default(),
        dns_name: node.dns_name.trim_end_matches('.').to_string(),
    })
}

/// A certificate and its private key, both PEM.
#[derive(Clone, PartialEq)]
pub struct TlsFiles {
    pub certificate: Vec<u8>,
    pub private_key: Vec<u8>,
}

/// This machine's HTTPS certificate from Tailscale, written into `directory`.
/// Tailscale reuses the certificate it has until it nears expiry, so running
/// this again is how it gets renewed.
pub fn tailscale_cert(dns_name: &str, directory: &Path) -> Result<TlsFiles, String> {
    tailscale_cert_with(dns_name, directory, run_tailscale)
}

fn tailscale_cert_with(
    dns_name: &str,
    directory: &Path,
    run: impl FnOnce(&[&str]) -> Result<Vec<u8>, String>,
) -> Result<TlsFiles, String> {
    std::fs::create_dir_all(directory)
        .map_err(|err| format!("Couldn't create {}: {err}", directory.display()))?;
    let certificate_path = directory.join("phone.crt");
    let key_path = directory.join("phone.key");
    let certificate_arg = certificate_path.to_string_lossy();
    let key_arg = key_path.to_string_lossy();
    run(&[
        "cert",
        "--cert-file",
        &certificate_arg,
        "--key-file",
        &key_arg,
        dns_name,
    ])?;
    set_owner_only(&key_path)?;
    let read = |path: &Path| {
        std::fs::read(path).map_err(|err| format!("Couldn't read {}: {err}", path.display()))
    };
    Ok(TlsFiles {
        certificate: read(&certificate_path)?,
        private_key: read(&key_path)?,
    })
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("Couldn't make {} private: {err}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// A machine on the tailnet that could run Mnemonic.
#[derive(Clone, Debug, PartialEq)]
pub struct TailnetDevice {
    /// The machine's MagicDNS name, used to reach it.
    pub host: String,
    /// The short machine name shown to people.
    pub name: String,
    pub ips: Vec<String>,
    pub login: String,
    pub owner_name: String,
    pub online: bool,
    pub is_this_machine: bool,
}

/// Every computer on the tailnet, this one first, then online machines,
/// each group by name. Phones, tablets and tagged servers are left out:
/// none of them can run Mnemonic or have a person's login to allow.
pub fn tailnet_devices() -> Result<Vec<TailnetDevice>, String> {
    let output = run_tailscale(&["status", "--json"])?;
    parse_tailnet_devices(&output)
}

fn parse_tailnet_devices(output: &[u8]) -> Result<Vec<TailnetDevice>, String> {
    let status: StatusJson = serde_json::from_slice(output)
        .map_err(|err| format!("Couldn't read Tailscale's status: {err}"))?;
    if status.backend_state != "Running" {
        return Err("Tailscale isn't connected".to_string());
    }
    let users = status.users.unwrap_or_default();
    let device = |node: StatusNode, is_this_machine: bool| -> Option<TailnetDevice> {
        if matches!(
            node.os.to_ascii_lowercase().as_str(),
            "ios" | "android" | "tvos"
        ) {
            return None;
        }
        let user = node.user_id.and_then(|id| users.get(&id.to_string()));
        let login = user
            .map(|user| user.login_name.clone())
            .filter(|login| !login.is_empty() && login != "tagged-devices")?;
        let ips = node.tailscale_ips.unwrap_or_default();
        let dns_name = node.dns_name.trim_end_matches('.').to_string();
        let host = if dns_name.is_empty() {
            ips.iter()
                .find(|ip| ip.parse::<Ipv4Addr>().is_ok())?
                .clone()
        } else {
            dns_name.clone()
        };
        let name = dns_name
            .split('.')
            .next()
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or(node.host_name);
        Some(TailnetDevice {
            host,
            name,
            ips,
            owner_name: user
                .map(|user| user.display_name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| login.clone()),
            login,
            online: node.online || is_this_machine,
            is_this_machine,
        })
    };
    let mut devices: Vec<TailnetDevice> = status
        .peers
        .unwrap_or_default()
        .into_values()
        .filter_map(|node| device(node, false))
        .collect();
    devices.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.cmp(&b.name)));
    if let Some(this_machine) = status.this_node.and_then(|node| device(node, true)) {
        devices.insert(0, this_machine);
    }
    Ok(devices)
}

#[derive(Deserialize)]
struct WhoisJson {
    #[serde(rename = "UserProfile")]
    user_profile: Option<WhoisUser>,
}

#[derive(Deserialize)]
struct WhoisUser {
    #[serde(rename = "LoginName", default)]
    login_name: String,
    #[serde(rename = "DisplayName", default)]
    display_name: String,
}

const WHOIS_CACHE_FOR: Duration = Duration::from_secs(60);

/// Identifies callers with `tailscale whois`, remembering answers for a
/// minute so a long-polling viewer doesn't run the CLI on every request.
#[derive(Default)]
pub struct TailscaleWhois {
    cache: Mutex<HashMap<IpAddr, (Option<TeamCaller>, Instant)>>,
}

impl CallerIdentity for TailscaleWhois {
    fn caller(&self, address: IpAddr) -> Result<Option<TeamCaller>, String> {
        {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((caller, at)) = cache.get(&address) {
                if at.elapsed() < WHOIS_CACHE_FOR {
                    return Ok(caller.clone());
                }
            }
        }
        let output = run_tailscale(&["whois", "--json", &address.to_string()])?;
        let whois: WhoisJson = serde_json::from_slice(&output)
            .map_err(|err| format!("Couldn't read Tailscale's whois answer: {err}"))?;
        let caller = whois
            .user_profile
            .filter(|user| !user.login_name.is_empty())
            .map(|user| {
                let name = single_line(&user.display_name);
                TeamCaller {
                    name: if name.is_empty() {
                        user.login_name.clone()
                    } else {
                        name
                    },
                    login: user.login_name,
                }
            });
        self.cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(address, (caller.clone(), Instant::now()));
        Ok(caller)
    }
}

/// A display name is chosen by its owner and ends up in chat labels and
/// notifications, so it must not be able to fake extra lines there.
fn single_line(name: &str) -> String {
    name.split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|character| !character.is_control())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Debug, PartialEq)]
pub enum TeamClientError {
    /// The machine couldn't be reached, which usually means it's asleep or
    /// sharing is off.
    Unreachable(String),
    /// The teammate hasn't allowed this login.
    Forbidden,
    /// The teammate runs a Mnemonic that speaks a different protocol.
    Incompatible {
        protocol: u32,
    },
    /// The teammate's app declined an action, with the status and the
    /// reason it gave.
    Refused {
        status: u16,
        reason: String,
    },
    Other(String),
}

impl std::fmt::Display for TeamClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(_) => write!(f, "Offline"),
            Self::Forbidden => write!(f, "Hasn't added you to their allow list"),
            Self::Incompatible { .. } => write!(f, "On a different Mnemonic version"),
            Self::Other(message) | Self::Refused { reason: message, .. } => {
                write!(f, "{message}")
            }
        }
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const FILE_TIMEOUT: Duration = Duration::from_secs(300);
/// Longer than the sharer waits for its app to take the batch (120 s), so
/// a batch taken just before then isn't reported failed after applying.
const BATCH_TIMEOUT: Duration = Duration::from_secs(150);
/// The longest a chat request waits for changes, shared by both ends.
pub const MAX_CHAT_WAIT: Duration = Duration::from_secs(25);

/// Talks to one teammate's Mnemonic. Blocking, so call it off the UI thread.
#[derive(Clone)]
pub struct TeamClient {
    http: reqwest::blocking::Client,
    base_url: String,
}

impl TeamClient {
    pub fn new(host: &str, default_port: u16) -> Result<Self, TeamClientError> {
        let base_url = team_base_url(host, default_port).map_err(TeamClientError::Other)?;
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .no_proxy()
            .build()
            .map_err(|err| TeamClientError::Other(err.to_string()))?;
        Ok(Self { http, base_url })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn get(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<reqwest::blocking::Response, TeamClientError> {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .timeout(timeout)
            .send()
            .map_err(|err| {
                if err.is_connect() || err.is_timeout() {
                    TeamClientError::Unreachable(err.to_string())
                } else {
                    TeamClientError::Other(err.to_string())
                }
            })?;
        match response.status().as_u16() {
            200..=299 | 404 => Ok(response),
            403 => Err(TeamClientError::Forbidden),
            status => {
                let body = response.text().unwrap_or_default();
                Err(TeamClientError::Other(if body.trim().is_empty() {
                    format!("Request failed ({status})")
                } else {
                    body.trim().to_string()
                }))
            }
        }
    }

    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<Option<T>, TeamClientError> {
        let response = self.get(path, timeout)?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let bytes = response
            .bytes()
            .map_err(|err| TeamClientError::Unreachable(err.to_string()))?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| TeamClientError::Other(format!("Unexpected answer: {err}")))
    }

    fn post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<reqwest::blocking::Response, TeamClientError> {
        self.post_json(path, &body, REQUEST_TIMEOUT)
    }

    fn post_json(
        &self,
        path: &str,
        body: &impl Serialize,
        timeout: Duration,
    ) -> Result<reqwest::blocking::Response, TeamClientError> {
        let response = self
            .http
            .post(format!("{}{path}", self.base_url))
            .timeout(timeout)
            .header(TEAM_REQUEST_HEADER, TEAM_PROTOCOL_VERSION.to_string())
            .json(body)
            .send()
            .map_err(|err| {
                if err.is_connect() || err.is_timeout() {
                    TeamClientError::Unreachable(err.to_string())
                } else {
                    TeamClientError::Other(err.to_string())
                }
            })?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(response);
        }
        let reason = match response.text() {
            Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
            Ok(_) => format!("Request failed ({status})"),
            Err(err) => {
                format!("Request failed ({status}), and its reason couldn't be read: {err}")
            }
        };
        Err(TeamClientError::Refused { status, reason })
    }

    /// Stops the chat's agent.
    pub fn stop(&self, chat_id: &str) -> Result<(), TeamClientError> {
        let path = format!("/v1/chats/{}/stop", path_segment(chat_id)?);
        self.post(&path, serde_json::json!({})).map(|_| ())
    }

    pub fn answer_permission(
        &self,
        chat_id: &str,
        tool_call_id: &str,
        option_id: &str,
    ) -> Result<(), TeamClientError> {
        let path = format!("/v1/chats/{}/permission", path_segment(chat_id)?);
        self.post(
            &path,
            serde_json::json!({ "tool_call_id": tool_call_id, "option_id": option_id }),
        )
        .map(|_| ())
    }

    /// Checks the teammate speaks this protocol, returning their name.
    pub fn hello(&self) -> Result<TeamHello, TeamClientError> {
        let hello: TeamHello = self
            .get_json("/v1/hello", REQUEST_TIMEOUT)?
            .ok_or_else(|| TeamClientError::Other("Not a Mnemonic".to_string()))?;
        check_hello(&hello)?;
        Ok(hello)
    }

    pub fn workspaces(&self) -> Result<Vec<TeamWorkspace>, TeamClientError> {
        let list: Option<TeamWorkspaceList> = self.get_json("/v1/workspaces", REQUEST_TIMEOUT)?;
        Ok(list.map(|list| list.workspaces).unwrap_or_default())
    }

    /// The workspace's whole diff, or None when it's no longer shared.
    pub fn diff(&self, workspace_id: &str) -> Result<Option<String>, TeamClientError> {
        let path = format!("/v1/workspaces/{}/diff", path_segment(workspace_id)?);
        let response = self.get(&path, Duration::from_secs(60))?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        response
            .text()
            .map(Some)
            .map_err(|err| TeamClientError::Unreachable(err.to_string()))
    }

    fn get_bytes(&self, path: &str, timeout: Duration) -> Result<Vec<u8>, TeamClientError> {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .timeout(timeout)
            .send()
            .map_err(|err| {
                if err.is_connect() || err.is_timeout() {
                    TeamClientError::Unreachable(err.to_string())
                } else {
                    TeamClientError::Other(err.to_string())
                }
            })?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let reason = match response.text() {
                Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
                _ => format!("Request failed ({status})"),
            };
            return Err(TeamClientError::Refused { status, reason });
        }
        response
            .bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|err| TeamClientError::Unreachable(err.to_string()))
    }

    /// Waits up to `wait` for the workspace's file manifest to change from
    /// `cursor`. None when there is no manifest yet or the workspace is
    /// unknown; sharing being off is `Err(Forbidden)`. The update's manifest
    /// is None when the cursor was still current.
    pub fn files(
        &self,
        workspace_id: &str,
        cursor: Option<&str>,
        wait: Duration,
    ) -> Result<Option<TeamFileUpdate>, TeamClientError> {
        let wait = wait.min(MAX_CHAT_WAIT);
        let mut path = format!(
            "/v1/workspaces/{}/files?wait={}",
            path_segment(workspace_id)?,
            wait.as_secs()
        );
        if let Some(cursor) = cursor {
            path.push_str("&cursor=");
            path.push_str(cursor);
        }
        self.get_json(&path, wait + REQUEST_TIMEOUT)
    }

    /// One file's bytes, as the sharer has it now.
    pub fn file(&self, workspace_id: &str, path: &str) -> Result<Vec<u8>, TeamClientError> {
        let url = format!(
            "/v1/workspaces/{}/file?path={}",
            path_segment(workspace_id)?,
            percent_encode(path)
        );
        self.get_bytes(&url, FILE_TIMEOUT)
    }

    /// A git bundle of the workspace's history, leaving out what the
    /// `have` commits already contain.
    pub fn bundle(&self, workspace_id: &str, have: &[String]) -> Result<Vec<u8>, TeamClientError> {
        let url = format!(
            "/v1/workspaces/{}/bundle?have={}",
            path_segment(workspace_id)?,
            have.join(",")
        );
        self.get_bytes(&url, FILE_TIMEOUT)
    }

    pub fn send_changes(
        &self,
        workspace_id: &str,
        batch: &TeamChangeBatch,
    ) -> Result<TeamBatchOutcome, TeamClientError> {
        let path = format!("/v1/workspaces/{}/changes", path_segment(workspace_id)?);
        let response = self.post_json(&path, batch, BATCH_TIMEOUT)?;
        let bytes = response
            .bytes()
            .map_err(|err| TeamClientError::Unreachable(err.to_string()))?;
        serde_json::from_slice(&bytes)
            .map_err(|err| TeamClientError::Other(format!("Unexpected answer: {err}")))
    }

    /// Reads one file through the workspace's agent endpoint, as the raw
    /// text of at most its first 2000 lines, each cut at 2000 characters.
    /// A trailing newline is not kept. None when the file doesn't exist.
    pub fn agent_read(
        &self,
        workspace_id: &str,
        absolute_path: &str,
    ) -> Result<Option<String>, TeamClientError> {
        let path = format!("/v1/workspaces/{}/agent", path_segment(workspace_id)?);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "Read", "arguments": {"file_path": absolute_path}},
        });
        let response = self.post_json(&path, &body, FILE_TIMEOUT)?;
        let bytes = response
            .bytes()
            .map_err(|err| TeamClientError::Unreachable(err.to_string()))?;
        let reply: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|err| TeamClientError::Other(format!("Unexpected answer: {err}")))?;
        if let Some(message) = reply
            .pointer("/error/message")
            .and_then(|message| message.as_str())
        {
            return Err(TeamClientError::Other(message.to_string()));
        }
        let text = reply
            .pointer("/result/content/0/text")
            .and_then(|text| text.as_str())
            .unwrap_or_default();
        if reply
            .pointer("/result/isError")
            .and_then(|flag| flag.as_bool())
            == Some(true)
        {
            if text == format!("{absolute_path} doesn't exist") {
                return Ok(None);
            }
            return Err(TeamClientError::Other(text.to_string()));
        }
        let lines: Vec<&str> = text
            .lines()
            .map(|line| line.split_once('\t').map_or(line, |(_, content)| content))
            .collect();
        Ok(Some(lines.join("\n")))
    }

    /// Waits up to `wait` for the chat to change after `cursor`. None when
    /// the chat isn't loaded in the teammate's app.
    pub fn chat(
        &self,
        chat_id: &str,
        cursor: Option<&str>,
        wait: Duration,
    ) -> Result<Option<TeamChatUpdate>, TeamClientError> {
        let wait = wait.min(MAX_CHAT_WAIT);
        let mut path = format!(
            "/v1/chats/{}?wait={}",
            path_segment(chat_id)?,
            wait.as_secs()
        );
        if let Some(cursor) = cursor {
            path.push_str("&cursor=");
            path.push_str(cursor);
        }
        self.get_json(&path, wait + REQUEST_TIMEOUT)
    }
}

fn check_hello(hello: &TeamHello) -> Result<(), TeamClientError> {
    if hello.protocol != TEAM_PROTOCOL_VERSION || hello.acp_crate != TEAM_ACP_CRATE_VERSION {
        return Err(TeamClientError::Incompatible {
            protocol: hello.protocol,
        });
    }
    Ok(())
}

/// Ids are generated by Mnemonic, so anything that would need escaping is a
/// bug rather than something to encode.
fn path_segment(id: &str) -> Result<&str, TeamClientError> {
    if is_plain_id(id) {
        Ok(id)
    } else {
        Err(TeamClientError::Other(format!("Unexpected id \"{id}\"")))
    }
}

fn percent_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

pub fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

pub fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// One file's section of a teammate's unified diff.
#[derive(Clone, Debug, PartialEq)]
pub struct TeamFileDiff {
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
    pub diff: String,
}

/// Rebuilds a chat from the markdown transcript saved when its turns ended,
/// one entry per `## User` or `## Assistant` section. Text before the first
/// section, the file's own heading, is dropped.
pub fn saved_chat_entries(markdown: &str) -> Vec<TeamEntry> {
    fn finish(section: Option<(bool, String)>, entries: &mut Vec<TeamEntry>) {
        let Some((from_user, text)) = section else {
            return;
        };
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let chunk = acp::ContentChunk::new(text.to_string().into());
        entries.push(TeamEntry::Updates {
            updates: vec![if from_user {
                acp::SessionUpdate::UserMessageChunk(chunk)
            } else {
                acp::SessionUpdate::AgentMessageChunk(chunk)
            }],
        });
    }

    let mut entries = Vec::new();
    let mut section: Option<(bool, String)> = None;
    for line in markdown.lines() {
        let heading = match line.trim_end() {
            "## User" | "## User (checkpoint)" => Some(true),
            "## Assistant" => Some(false),
            _ => None,
        };
        match heading {
            Some(from_user) => {
                finish(section.take(), &mut entries);
                section = Some((from_user, String::new()));
            }
            None => {
                if let Some((_, text)) = section.as_mut() {
                    text.push_str(line);
                    text.push('\n');
                }
            }
        }
    }
    finish(section, &mut entries);
    entries
}

/// Splits a unified diff into one item per `diff --git` section.
pub fn split_unified_diff(diff: &str) -> Vec<TeamFileDiff> {
    let mut files: Vec<TeamFileDiff> = Vec::new();
    let mut in_hunk = false;
    let mut old_path: Option<String> = None;
    let mut new_path: Option<String> = None;
    let mut finished_paths: Vec<(Option<String>, Option<String>)> = Vec::new();

    for line in diff.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        if let Some(header) = text.strip_prefix("diff --git ") {
            if !files.is_empty() {
                finished_paths.push((old_path.take(), new_path.take()));
            }
            in_hunk = false;
            old_path = None;
            new_path = None;
            let fallback = header
                .rsplit_once(" b/")
                .map(|(_, path)| path.to_string())
                .unwrap_or_else(|| header.to_string());
            files.push(TeamFileDiff {
                path: fallback,
                additions: 0,
                deletions: 0,
                diff: String::new(),
            });
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        file.diff.push_str(line);
        if text.starts_with("@@") {
            in_hunk = true;
        } else if !in_hunk {
            if let Some(path) = text.strip_prefix("--- ") {
                old_path = path.strip_prefix("a/").map(str::to_string);
            } else if let Some(path) = text.strip_prefix("+++ ") {
                new_path = path.strip_prefix("b/").map(str::to_string);
            }
        } else if text.starts_with('+') {
            file.additions += 1;
        } else if text.starts_with('-') {
            file.deletions += 1;
        }
    }
    if !files.is_empty() {
        finished_paths.push((old_path, new_path));
    }
    for (file, (old_path, new_path)) in files.iter_mut().zip(finished_paths) {
        if let Some(path) = new_path.or(old_path) {
            file.path = path;
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_unified_diff_lists_each_file_with_counts() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\nindex 1..2 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n keep\n-old\n+new\n+more\ndiff --git a/notes.md b/notes.md\nnew file mode 100644\n--- /dev/null\n+++ b/notes.md\n@@ -0,0 +1 @@\n+hello\n";
        let files = split_unified_diff(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!((files[0].additions, files[0].deletions), (2, 1));
        assert!(files[0].diff.starts_with("diff --git a/src/lib.rs"));
        assert!(!files[0].diff.contains("notes.md"));
        assert_eq!(files[1].path, "notes.md");
        assert_eq!((files[1].additions, files[1].deletions), (1, 0));
    }

    #[test]
    fn split_unified_diff_names_deleted_files() {
        let diff = "diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n";
        let files = split_unified_diff(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "gone.txt");
        assert_eq!((files[0].additions, files[0].deletions), (0, 2));
    }

    fn user(text: &str) -> TeamEntry {
        TeamEntry::Updates {
            updates: vec![acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(
                text.into(),
            ))],
        }
    }

    #[test]
    fn a_saved_transcript_becomes_one_entry_per_turn() {
        let saved = "# Fix login\n\nA chat from the workspace \"crisp-panda\".\n\n## User\n\nfix it\n\n## Assistant\n\nDone.\n\n**Tool Call: Edit**\n\n## User (checkpoint)\n\nthanks\n\n";
        let agent = TeamEntry::Updates {
            updates: vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new("Done.\n\n**Tool Call: Edit**".into()),
            )],
        };
        assert_eq!(
            saved_chat_entries(saved),
            vec![user("fix it"), agent, user("thanks")]
        );
    }

    #[test]
    fn a_cursor_returns_only_later_changes_and_a_replaced_chat_resets_it() {
        let hub = TeamShareHub::new();
        hub.replace_chat("chat", vec![user("one")]);
        let first = hub.chat_update("chat", None).expect("chat is open");
        assert!(first.reset);
        assert_eq!(first.changes.len(), 1);

        assert!(hub.set_chat_entry("chat", 1, user("two")));
        let second = hub
            .chat_update("chat", Some(&first.cursor))
            .expect("chat is open");
        assert!(!second.reset);
        assert_eq!(
            second.changes,
            vec![TeamEntryChange {
                index: 1,
                entry: user("two")
            }]
        );

        hub.replace_chat("chat", vec![user("fresh")]);
        let third = hub
            .chat_update("chat", Some(&second.cursor))
            .expect("chat is open");
        assert!(third.reset);
        assert_eq!(third.len, 1);

        assert!(!hub.set_chat_entry("chat", 5, user("gap")));
        hub.remove_chat("chat");
        assert!(hub.chat_update("chat", Some(&third.cursor)).is_none());
    }

    #[test]
    fn set_chat_plan_bumps_only_on_change() {
        let hub = TeamShareHub::new();
        hub.replace_chat("chat", vec![user("one")]);
        let first = hub.chat_update("chat", None).expect("chat is open");
        let plan = vec![acp::PlanEntry::new(
            "Fix the form",
            acp::PlanEntryPriority::High,
            acp::PlanEntryStatus::InProgress,
        )];
        assert!(hub.set_chat_plan("chat", plan.clone()));

        let waited_from = Instant::now();
        let second = hub
            .wait_chat_update("chat", Some(&first.cursor), Duration::from_secs(10))
            .expect("chat is open");
        assert!(
            waited_from.elapsed() < Duration::from_secs(5),
            "a plan-only change should end the long-poll at once"
        );
        assert!(!second.reset);
        assert!(second.changes.is_empty());
        assert_eq!(second.plan, plan);
        assert_ne!(second.cursor, first.cursor);

        assert!(hub.set_chat_plan("chat", plan));
        let third = hub
            .chat_update("chat", Some(&second.cursor))
            .expect("chat is open");
        assert_eq!(third.cursor, second.cursor);
        assert!(!hub.set_chat_plan("closed", Vec::new()));
    }

    #[test]
    fn chat_activity_bumps_only_on_change() {
        let hub = TeamShareHub::new();
        hub.replace_chat("chat", vec![user("one")]);
        let first = hub.chat_update("chat", None).expect("chat is open");
        assert!(!first.generating);
        assert!(hub.set_chat_activity("chat", true, Vec::new()));

        let second = hub
            .wait_chat_update("chat", Some(&first.cursor), Duration::ZERO)
            .expect("chat is open");
        assert!(second.generating);
        assert!(!second.reset);
        assert!(second.changes.is_empty());
        assert_ne!(second.cursor, first.cursor);

        assert!(hub.set_chat_activity("chat", true, Vec::new()));
        let third = hub
            .wait_chat_update("chat", Some(&second.cursor), Duration::ZERO)
            .expect("chat is open");
        assert_eq!(third.cursor, second.cursor);
        assert!(!hub.set_chat_activity("missing", true, Vec::new()));
    }

    #[test]
    fn acp_crate_version_matches_the_dependency_pin() {
        let workspace_manifest = include_str!("../../../../Cargo.toml");
        let pin = format!("agent-client-protocol = {{ version = \"={TEAM_ACP_CRATE_VERSION}\"");
        assert!(
            workspace_manifest.contains(&pin),
            "The workspace no longer pins `{pin}`; update TEAM_ACP_CRATE_VERSION to match"
        );
    }

    #[test]
    fn hello_rejects_a_different_acp_version() {
        let hello = |acp_crate: &str| TeamHello {
            name: "Alice".to_string(),
            protocol: TEAM_PROTOCOL_VERSION,
            acp_crate: acp_crate.to_string(),
        };
        assert_eq!(
            check_hello(&hello("0.13.0")),
            Err(TeamClientError::Incompatible {
                protocol: TEAM_PROTOCOL_VERSION
            })
        );
        assert_eq!(check_hello(&hello(TEAM_ACP_CRATE_VERSION)), Ok(()));
    }

    #[test]
    fn reads_this_machines_address_and_login_from_tailscale_status() {
        let status = br#"{
            "BackendState": "Running",
            "Self": { "TailscaleIPs": ["fd7a:115c:a1e0::1", "100.101.102.103"], "UserID": 42 },
            "User": { "42": { "LoginName": "jay@example.com", "DisplayName": "Jay" } }
        }"#;
        assert_eq!(
            parse_tailscale_status(status),
            Ok(TailscaleSelf {
                ip: Ipv4Addr::new(100, 101, 102, 103),
                login: "jay@example.com".to_string(),
                display_name: "Jay".to_string(),
                dns_name: String::new(),
            })
        );
        assert!(parse_tailscale_status(br#"{"BackendState": "Stopped"}"#).is_err());
    }

    #[test]
    fn status_reports_the_magic_dns_name() {
        let status = br#"{
            "BackendState": "Running",
            "Self": { "TailscaleIPs": ["100.101.102.103"], "UserID": 42,
                      "DNSName": "jay-mbp.tail1234.ts.net." }
        }"#;
        assert_eq!(
            parse_tailscale_status(status).map(|status| status.dns_name),
            Ok("jay-mbp.tail1234.ts.net".to_string())
        );
    }

    #[test]
    fn lists_tailnet_computers_with_their_owners_and_skips_phones_and_servers() {
        let status = br#"{
            "BackendState": "Running",
            "Self": { "TailscaleIPs": ["100.64.0.1"], "UserID": 1, "HostName": "Jay's MacBook Pro",
                      "DNSName": "jays-mbp.tail1234.ts.net.", "OS": "macOS" },
            "Peer": {
                "a": { "TailscaleIPs": ["100.64.0.2"], "UserID": 2, "DNSName": "alice-mbp.tail1234.ts.net.",
                       "OS": "macOS", "Online": false },
                "b": { "TailscaleIPs": ["100.64.0.3"], "UserID": 1, "DNSName": "jay-pc.tail1234.ts.net.",
                       "OS": "windows", "Online": true },
                "c": { "TailscaleIPs": ["100.64.0.4"], "UserID": 1, "DNSName": "jay-phone.tail1234.ts.net.",
                       "OS": "iOS", "Online": true },
                "d": { "TailscaleIPs": ["100.64.0.5"], "UserID": 3, "DNSName": "ci.tail1234.ts.net.",
                       "OS": "linux", "Online": true }
            },
            "User": {
                "1": { "LoginName": "jay@example.com", "DisplayName": "Jay" },
                "2": { "LoginName": "alice@example.com", "DisplayName": "Alice" },
                "3": { "LoginName": "tagged-devices", "DisplayName": "tagged-devices" }
            }
        }"#;
        let devices = parse_tailnet_devices(status).expect("status parses");
        let summary: Vec<(&str, &str, &str, bool, bool)> = devices
            .iter()
            .map(|device| {
                (
                    device.name.as_str(),
                    device.host.as_str(),
                    device.login.as_str(),
                    device.online,
                    device.is_this_machine,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "jays-mbp",
                    "jays-mbp.tail1234.ts.net",
                    "jay@example.com",
                    true,
                    true
                ),
                (
                    "jay-pc",
                    "jay-pc.tail1234.ts.net",
                    "jay@example.com",
                    true,
                    false
                ),
                (
                    "alice-mbp",
                    "alice-mbp.tail1234.ts.net",
                    "alice@example.com",
                    false,
                    false
                ),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn tailscale_cert_keeps_the_key_private() -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().map_err(|err| err.to_string())?;
        let phone_directory = directory.path().join("phone");
        let files = tailscale_cert_with("mac.example.ts.net", &phone_directory, |args| {
            let [
                "cert",
                "--cert-file",
                certificate,
                "--key-file",
                key,
                "mac.example.ts.net",
            ] = args
            else {
                return Err(format!("unexpected arguments {args:?}"));
            };
            std::fs::write(certificate, "certificate").map_err(|err| err.to_string())?;
            std::fs::write(key, "key").map_err(|err| err.to_string())?;
            std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o644))
                .map_err(|err| err.to_string())?;
            Ok(Vec::new())
        })?;
        assert!(files.certificate == b"certificate" && files.private_key == b"key");
        let mode = std::fs::metadata(phone_directory.join("phone.key"))
            .map_err(|err| err.to_string())?
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let failed = tailscale_cert_with("mac.example.ts.net", &phone_directory, |_| {
            Err("HTTPS is not enabled for this tailnet".to_string())
        });
        assert!(
            failed.is_err_and(|err| err == "HTTPS is not enabled for this tailnet"),
            "the error is tailscale's own words"
        );
        Ok(())
    }
}
