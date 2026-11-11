//! Read-only sharing of workspaces and live agent chats between teammates on
//! the same Tailscale network. Each Mnemonic serves its own state (see
//! `team_share_server`); there is no central server.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Bumped whenever a change to the wire types would break an older peer.
pub const TEAM_PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_TEAM_PORT: u16 = 47231;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamHello {
    pub name: String,
    pub protocol: u32,
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
    /// Whether the chat is loaded in the sharer's app, which is the only
    /// time its transcript can be read.
    pub open: bool,
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamWorkspaceList {
    pub workspaces: Vec<TeamWorkspace>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamToolStatus {
    Pending,
    WaitingForApproval,
    Running,
    Done,
    Failed,
    Rejected,
    Canceled,
}

/// One entry of a chat transcript. Deliberately independent of ACP's types
/// so an ACP upgrade on one machine doesn't break the wire format.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamEntry {
    User {
        markdown: String,
    },
    Assistant {
        markdown: String,
        /// The model's thinking before or between its reply.
        #[serde(default)]
        thinking: Option<String>,
    },
    ToolCall {
        title: String,
        tool_kind: String,
        status: TeamToolStatus,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        output_preview: Option<String>,
    },
    Plan {
        markdown: String,
    },
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
}

struct HubState {
    /// Random per process, so a viewer's cursor from before a restart of
    /// the sharer's app is never mistaken for a current one.
    epoch: u64,
    version: u64,
    workspaces: Vec<TeamWorkspace>,
    chats: HashMap<String, ChatFeed>,
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
        Self::update_locked(&state, chat_id, cursor)
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
            let update = Self::update_locked(&state, chat_id, cursor)?;
            if update.reset || !update.changes.is_empty() {
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

    fn update_locked(
        state: &HubState,
        chat_id: &str,
        cursor: Option<&str>,
    ) -> Option<TeamChatUpdate> {
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
        let changes = feed
            .entries
            .iter()
            .enumerate()
            .filter(|(_, (_, changed_at))| since.is_none_or(|since| *changed_at > since))
            .map(|(index, (entry, _))| TeamEntryChange {
                index,
                entry: entry.clone(),
            })
            .collect();
        Some(TeamChatUpdate {
            cursor: format!("{}.{}.{}", state.epoch, feed.generation, state.version),
            reset: since.is_none(),
            len: feed.entries.len(),
            changes,
        })
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
    /// The caller's login, None when the address isn't a known user (such as
    /// a tagged machine), or an error when it couldn't be checked.
    fn login(&self, address: IpAddr) -> Result<Option<String>, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct TailscaleSelf {
    pub ip: Ipv4Addr,
    pub login: String,
    pub display_name: String,
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
    })
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
}

const WHOIS_CACHE_FOR: Duration = Duration::from_secs(60);

/// Identifies callers with `tailscale whois`, remembering answers for a
/// minute so a long-polling viewer doesn't run the CLI on every request.
#[derive(Default)]
pub struct TailscaleWhois {
    cache: Mutex<HashMap<IpAddr, (Option<String>, Instant)>>,
}

impl CallerIdentity for TailscaleWhois {
    fn login(&self, address: IpAddr) -> Result<Option<String>, String> {
        {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((login, at)) = cache.get(&address) {
                if at.elapsed() < WHOIS_CACHE_FOR {
                    return Ok(login.clone());
                }
            }
        }
        let output = run_tailscale(&["whois", "--json", &address.to_string()])?;
        let whois: WhoisJson = serde_json::from_slice(&output)
            .map_err(|err| format!("Couldn't read Tailscale's whois answer: {err}"))?;
        let login = whois
            .user_profile
            .map(|user| user.login_name)
            .filter(|login| !login.is_empty());
        self.cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(address, (login.clone(), Instant::now()));
        Ok(login)
    }
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
    Other(String),
}

impl std::fmt::Display for TeamClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(_) => write!(f, "Offline"),
            Self::Forbidden => write!(f, "Hasn't added you to their allow list"),
            Self::Incompatible { .. } => write!(f, "On a different Mnemonic version"),
            Self::Other(message) => write!(f, "{message}"),
        }
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
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

    /// Checks the teammate speaks this protocol, returning their name.
    pub fn hello(&self) -> Result<TeamHello, TeamClientError> {
        let hello: TeamHello = self
            .get_json("/v1/hello", REQUEST_TIMEOUT)?
            .ok_or_else(|| TeamClientError::Other("Not a Mnemonic".to_string()))?;
        if hello.protocol != TEAM_PROTOCOL_VERSION {
            return Err(TeamClientError::Incompatible {
                protocol: hello.protocol,
            });
        }
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

/// Ids are generated by Mnemonic, so anything that would need escaping is a
/// bug rather than something to encode.
fn path_segment(id: &str) -> Result<&str, TeamClientError> {
    if is_plain_id(id) {
        Ok(id)
    } else {
        Err(TeamClientError::Other(format!("Unexpected id \"{id}\"")))
    }
}

pub fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> TeamEntry {
        TeamEntry::User {
            markdown: text.to_string(),
        }
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
            })
        );
        assert!(parse_tailscale_status(br#"{"BackendState": "Stopped"}"#).is_err());
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
}
